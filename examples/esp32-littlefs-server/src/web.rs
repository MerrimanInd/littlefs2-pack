use embassy_net::Stack;
use embassy_time::Duration;
use esp_alloc as _;
use picoserve::{AppBuilder, AppRouter, Router, response::File, routing};

use crate::fs::FileServer;

// ── Web Application ─────────────────────────────

pub struct Application {
    pub file_server: &'static FileServer,
}

impl AppBuilder for Application {
    type PathRouter = impl routing::PathRouter;

    fn build_app(self) -> picoserve::Router<Self::PathRouter> {
        let fs = self.file_server;

        let index = fs.get_str("/index.html").expect("missing /index.html");

        picoserve::Router::new().route("/", routing::get_service(File::html(index)))
        // ── Add routes for your other files here ──────────────
        // .route("/style.css", routing::get_service(
        //     File::css(fs.get_str("/style.css").unwrap())
        // ))
        // .route("/app.js", routing::get_service(
        //     File::javascript(fs.get_str("/js/app.js").unwrap())
        // ))
    }
}

pub const WEB_TASK_POOL_SIZE: usize = 2;

pub struct WebApp {
    pub router: &'static Router<<Application as AppBuilder>::PathRouter>,
    pub config: &'static picoserve::Config,
}

impl WebApp {
    pub fn new(file_server: &'static FileServer) -> Self {
        let app = Application { file_server };
        let router = picoserve::make_static!(AppRouter<Application>, app.build_app());

        let config = picoserve::make_static!(
            picoserve::Config,
            picoserve::Config::new(picoserve::Timeouts {
                start_read_request: Duration::from_secs(5),
                read_request: Duration::from_secs(5),
                write: Duration::from_secs(1),
                persistent_start_read_request: Duration::from_secs(1),
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
    let mut tcp_tx_buffer = [0; 1024];
    let mut http_buffer = [0; 2048];

    picoserve::Server::new(router, config, &mut http_buffer)
        .listen_and_serve(task_id, stack, port, &mut tcp_rx_buffer, &mut tcp_tx_buffer)
        .await
        .into_never()
}
