// ESP-Dashboard — Main JavaScript
document.addEventListener('DOMContentLoaded', () => {
    // Animate elements on scroll
    const observer = new IntersectionObserver((entries) => {
        entries.forEach(entry => {
            if (entry.isIntersecting) {
                entry.target.classList.add('animate');
                observer.unobserve(entry.target);
            }
        });
    }, { threshold: 0.1 });

    document.querySelectorAll('.card, .sensor-card, .chart-container, .gallery-item').forEach(el => {
        el.style.opacity = '0';
        observer.observe(el);
    });

    // Active nav link
    const currentPage = window.location.pathname.split('/').pop() || 'index.html';
    document.querySelectorAll('.nav-links a').forEach(link => {
        if (link.getAttribute('href').includes(currentPage)) {
            link.classList.add('active');
        }
    });

    // Live clock
    const clockEl = document.getElementById('live-clock');
    if (clockEl) {
        setInterval(() => {
            clockEl.textContent = new Date().toLocaleTimeString();
        }, 1000);
    }

    // Simulated live sensor values
    document.querySelectorAll('[data-sensor]').forEach(el => {
        const base = parseFloat(el.dataset.base || 20);
        const range = parseFloat(el.dataset.range || 5);
        const unit = el.dataset.unit || '';
        setInterval(() => {
            const val = (base + (Math.random() - 0.5) * range).toFixed(1);
            el.textContent = val + unit;
        }, 2000 + Math.random() * 3000);
    });

    console.log('ESP-Dashboard loaded.');
});

// Tab switching for docs
function showTab(tabId) {
    document.querySelectorAll('.tab-content').forEach(t => t.style.display = 'none');
    document.querySelectorAll('.tab-btn').forEach(b => b.classList.remove('active'));
    const tab = document.getElementById(tabId);
    if (tab) tab.style.display = 'block';
    event.target.classList.add('active');
}
