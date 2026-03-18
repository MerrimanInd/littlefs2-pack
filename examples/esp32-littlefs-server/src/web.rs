use embassy_net::Stack;
use embassy_time::Duration;
use picoserve::{
    AppBuilder, AppRouter, ResponseSent, Router,
    io::{Read, Write},
    request::{Path, Request},
    response::{Content, IntoResponse, ResponseWriter, StatusCode},
    routing::{self, PathRouterService},
};

// ── Content wrapper with correct MIME type ──────────────────────────────
//
// picoserve's Content impl for &[u8] hardcodes Content-Type to
// "application/octet-stream". If we use the tuple IntoResponse like
//   (StatusCode::OK, ("Content-Type", "text/css"), data.as_slice())
// we get TWO Content-Type headers — the browser sees the first one
// (application/octet-stream) and ignores our CSS/JS/HTML type.
//
// This wrapper is exactly what picoserve's own File type does internally:
// implement Content to return the correct content_type().

struct FileContent<'a> {
    content_type: &'static str,
    data: &'a [u8],
}

impl Content for FileContent<'_> {
    fn content_type(&self) -> &'static str {
        self.content_type
    }

    fn content_length(&self) -> usize {
        self.data.len()
    }

    async fn write_content<W: Write>(self, mut writer: W) -> Result<(), W::Error> {
        writer.write_all(self.data).await
    }
}

// ── On-demand file server ───────────────────────────────────────────────
//
// Instead of loading every file into PSRAM at startup and leaking it
// forever, this struct implements picoserve's PathRouterService trait.
//
// On each request it:
//   1. Extracts the URL path
//   2. Normalizes it (strip leading /, default to index.html)
//   3. Opens the file in LittleFS and reads it into a temporary Vec
//      on internal SRAM (~1-50 KB typical, freed after response)
//   4. Writes the response with correct Content-Type
//   5. Drops the Vec — memory is reclaimed immediately
//
// Net effect: only one file's worth of RAM is used at a time per
// connection, instead of ALL files permanently leaked into PSRAM.

pub struct LittleFsFileServer;

impl<State, CurrentPathParameters> PathRouterService<State, CurrentPathParameters>
    for LittleFsFileServer
{
    async fn call_path_router_service<R: Read, W: ResponseWriter<Error = R::Error>>(
        &self,
        _state: &State,
        _current_path_parameters: CurrentPathParameters,
        path: Path<'_>,
        request: Request<'_, R>,
        response_writer: W,
    ) -> Result<ResponseSent, W::Error> {
        let url_path = path.encoded();

        // Finalize the request body (discards any unread body bytes)
        // to get the underlying Connection, which write_to needs.
        // This is exactly what picoserve's own File impl does.
        let connection = request.body_connection.finalize().await?;

        // Normalize and validate the path
        let lfs_path = match crate::fs::normalize_url_path(url_path) {
            Some(p) => p,
            None => {
                return (StatusCode::FORBIDDEN, "Forbidden")
                    .write_to(connection, response_writer)
                    .await;
            }
        };

        // Safety: we just built this from valid UTF-8 segments
        let path_str = core::str::from_utf8(&lfs_path).unwrap_or("/index.html");

        // Read the file from flash into a temporary Vec on internal SRAM.
        // The Vec is freed when this function returns.
        match crate::fs::read_file(path_str) {
            Some(data) => {
                let ct = crate::fs::content_type(path_str);
                FileContent {
                    content_type: ct,
                    data: data.as_slice(),
                }
                .write_to(connection, response_writer)
                .await
            }
            None => {
                (StatusCode::NOT_FOUND, "Not Found")
                    .write_to(connection, response_writer)
                    .await
            }
        }
    }
}

// ── Web Application ─────────────────────────────

pub struct Application;

impl AppBuilder for Application {
    type PathRouter = impl routing::PathRouter;

    fn build_app(self) -> picoserve::Router<Self::PathRouter> {
        // from_service forwards ALL requests with the full path
        // directly to our file server — no prefix stripping.
        //
        // If you need API routes alongside the file server, use:
        //
        //   Router::new()
        //       .route("/api/status", routing::get(handle_status))
        //       .nest_service("", LittleFsFileServer)
        //
        // Note: the prefix must be "" (empty), NOT "/".
        // nest_service("/") strips the "/" and then rejects paths
        // with nothing remaining (like the root request).
        Router::from_service(LittleFsFileServer)
    }
}

// Browsers open 6+ connections per host. 4 tasks is a practical
// compromise on ESP32: each task uses ~8KB of stack+buffers
// (4KB tx + 1KB rx + 2KB http + ~1KB stack) = ~32KB total.
pub const WEB_TASK_POOL_SIZE: usize = 4;

pub struct WebApp {
    pub router: &'static Router<<Application as AppBuilder>::PathRouter>,
    pub config: &'static picoserve::Config,
}

impl Default for WebApp {
    fn default() -> Self {
        let router = picoserve::make_static!(AppRouter<Application>, Application.build_app());

        let config = picoserve::make_static!(
            picoserve::Config,
            picoserve::Config::new(picoserve::Timeouts {
                start_read_request: Duration::from_secs(5),
                read_request: Duration::from_secs(5),
                // Writing a 20KB CSS file through a 4KB TCP buffer
                // at Wi-Fi speeds can easily take several seconds.
                // 30s is generous but prevents dropped resources.
                write: Duration::from_secs(30),
                // Keep-alive: browser reuses connections for CSS/JS/images.
                // 5s gives the browser plenty of time to pipeline the
                // next request on an existing connection.
                persistent_start_read_request: Duration::from_secs(5),
            })
            .keep_connection_alive()
        );

        Self { router, config }
    }
}

#[embassy_executor::task(pool_size = WEB_TASK_POOL_SIZE)]
pub async fn web_task(
    task_id: usize,
    stack: Stack<'static>,
    router: &'static AppRouter<Application>,
    config: &'static picoserve::Config,
) -> ! {
    let port = 80;
    let mut tcp_rx_buffer = [0; 1024];
    // Larger TX buffer: fewer TCP segments per file, fewer round-trips.
    // 4KB ≈ 2-3 TCP segments for a typical CSS/JS file, and fits
    // comfortably in internal SRAM (4KB × 2 tasks = 8KB).
    let mut tcp_tx_buffer = [0; 4096];
    let mut http_buffer = [0; 2048];

    picoserve::Server::new(router, config, &mut http_buffer)
        .listen_and_serve(task_id, stack, port, &mut tcp_rx_buffer, &mut tcp_tx_buffer)
        .await
        .into_never()
}
