/**
 * Framework-light WebSocket client for the /ws hub.
 *
 * - Auto-reconnects with exponential backoff (250ms doubling, capped at 8s,
 *   jittered) and resubscribes to channels on every (re)open.
 * - Treats the server's periodic `health.snapshot` broadcast as a heartbeat:
 *   if no message arrives within HEARTBEAT_STALE_MS the socket is recycled so
 *   a silently-dead connection is replaced by a fresh one. The hub only routes
 *   `health.*` events to subscribers of the "health" channel, so that channel
 *   is always added to the subscription set.
 * - `onStatus` reports connection state for UI badges.
 */

export interface WsEventMessage {
	ts?: string;
	type?: string;
	payload?: unknown;
}

export type WsStatus = 'connecting' | 'connected' | 'disconnected' | 'error';

export type WsHandle = { close: () => void };

export interface ConnectWSOptions {
	channels?: string[];
	onEvent?: (msg: WsEventMessage) => void;
	onStatus?: (status: WsStatus) => void;
}

const MIN_BACKOFF_MS = 250;
const MAX_BACKOFF_MS = 8000;
// Server publishes health.snapshot every ~10s; allow two missed beats plus slack.
const HEARTBEAT_STALE_MS = 35000;
const STALENESS_CHECK_INTERVAL_MS = 5000;

type TimerId = NodeJS.Timeout | number;

/**
 * Pure backoff computation — exported as a test seam.
 * Returns the delay in ms, jittered within [base/2, base].
 */
export function nextBackoffMs(attempt: number, rand: () => number = Math.random): number {
	const base = Math.min(MAX_BACKOFF_MS, MIN_BACKOFF_MS * Math.pow(2, attempt));
	return Math.round(base * (0.5 + rand() * 0.5));
}

export function connectWS({ channels = [], onEvent, onStatus }: ConnectWSOptions = {}): WsHandle {
	if (typeof window === 'undefined' || typeof WebSocket === 'undefined') {
		// SSR/no-WS environment: inert handle.
		onStatus?.('disconnected');
		return { close() {} };
	}

	let closed = false;
	let attempts = 0;
	let socket: WebSocket | null = null;
	let reconnectTimer: TimerId | null = null;
	let stalenessTimer: TimerId | null = null;
	let lastActivityAt = 0;

	function setStatus(status: WsStatus) {
		onStatus?.(status);
	}

	function noteActivity() {
		lastActivityAt = Date.now();
	}

	function stopStalenessWatch() {
		if (stalenessTimer !== null) {
			clearInterval(stalenessTimer);
			stalenessTimer = null;
		}
	}

	function clearTimers() {
		if (reconnectTimer !== null) {
			clearTimeout(reconnectTimer);
			reconnectTimer = null;
		}
		stopStalenessWatch();
	}

	function open() {
		if (closed) return;
		clearTimers();
		const proto = window.location.protocol === 'https:' ? 'wss' : 'ws';
		try {
			socket = new WebSocket(`${proto}://${window.location.host}/ws`);
		} catch {
			socket = null;
			setStatus('error');
			scheduleReconnect();
			return;
		}
		setStatus('connecting');

		socket.onopen = () => {
			attempts = 0;
			noteActivity();
			setStatus('connected');
			try {
				socket?.send(
					JSON.stringify({
						type: 'subscribe',
						channels: Array.from(new Set([...channels, 'health']))
					})
				);
			} catch {
				// Socket died between open and send; onclose handles recovery.
			}
			stopStalenessWatch();
			stalenessTimer = setInterval(() => {
				if (closed) return;
				if (Date.now() - lastActivityAt > HEARTBEAT_STALE_MS) {
					// Force-close; onclose schedules a fresh, resubscribed connection.
					try {
						socket?.close();
					} catch {
						// ignore
					}
				}
			}, STALENESS_CHECK_INTERVAL_MS);
		};

		socket.onmessage = (ev: MessageEvent) => {
			noteActivity();
			let msg: unknown = null;
			try {
				msg = JSON.parse(typeof ev.data === 'string' ? ev.data : '');
			} catch {
				return;
			}
			if (msg && typeof msg === 'object') onEvent?.(msg as WsEventMessage);
		};

		socket.onerror = () => {
			setStatus('error');
		};

		socket.onclose = () => {
			socket = null;
			stopStalenessWatch();
			scheduleReconnect();
		};
	}

	function scheduleReconnect() {
		if (closed || reconnectTimer !== null) return;
		setStatus('disconnected');
		const delay = nextBackoffMs(attempts);
		attempts += 1;
		reconnectTimer = setTimeout(() => {
			reconnectTimer = null;
			open();
		}, delay);
	}

	open();

	return {
		close() {
			closed = true;
			clearTimers();
			if (socket) {
				const s = socket;
				socket = null;
				s.onclose = null;
				s.onerror = null;
				s.onmessage = null;
				s.onopen = null;
				try {
					s.close();
				} catch {
					// ignore
				}
			}
			setStatus('disconnected');
		}
	};
}
