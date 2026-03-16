// Utility functions for ESP-Dashboard
const SensorUtils = {
    convertTemp(c, unit) {
        if (unit === 'F') return (c * 9/5) + 32;
        if (unit === 'K') return c + 273.15;
        return c;
    },
    humidityDewpoint(temp, humidity) {
        const a = 17.27, b = 237.7;
        const alpha = (a * temp) / (b + temp) + Math.log(humidity / 100);
        return (b * alpha) / (a - alpha);
    },
    pressureAltitude(pressure, seaLevel = 1013.25) {
        return 44330 * (1 - Math.pow(pressure / seaLevel, 0.1903));
    },
    movingAverage(data, window = 5) {
        return data.map((_, i, arr) => {
            const start = Math.max(0, i - window + 1);
            const slice = arr.slice(start, i + 1);
            return slice.reduce((a, b) => a + b, 0) / slice.length;
        });
    },
    formatUptime(hours) {
        const days = Math.floor(hours / 24);
        const hrs = hours % 24;
        return `${days}d ${hrs}h`;
    },
    rssiToQuality(rssi) {
        if (rssi >= -50) return 'Excellent';
        if (rssi >= -60) return 'Good';
        if (rssi >= -70) return 'Fair';
        return 'Poor';
    }
};

const ChartHelper = {
    generateGradient(ctx, color, height) {
        const gradient = ctx.createLinearGradient(0, 0, 0, height);
        gradient.addColorStop(0, color + '40');
        gradient.addColorStop(1, color + '00');
        return gradient;
    },
    formatTimestamp(ts) {
        return new Date(ts * 1000).toLocaleString();
    },
    downsample(data, targetPoints) {
        if (data.length <= targetPoints) return data;
        const step = Math.ceil(data.length / targetPoints);
        return data.filter((_, i) => i % step === 0);
    }
};

// WebSocket mock for demo
class SensorWebSocket {
    constructor(url) {
        this.url = url;
        this.callbacks = {};
        this.connected = false;
    }
    on(event, cb) { this.callbacks[event] = cb; }
    connect() {
        this.connected = true;
        if (this.callbacks.open) this.callbacks.open();
        this._simulate();
    }
    _simulate() {
        if (!this.connected) return;
        const data = {
            temperature: 20 + Math.random() * 10,
            humidity: 40 + Math.random() * 30,
            pressure: 1010 + Math.random() * 10,
            timestamp: Date.now()
        };
        if (this.callbacks.message) this.callbacks.message(data);
        setTimeout(() => this._simulate(), 1000 + Math.random() * 2000);
    }
    disconnect() { this.connected = false; }
}

if (typeof module !== 'undefined') module.exports = { SensorUtils, ChartHelper, SensorWebSocket };
