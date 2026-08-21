/**
 * Framework-light API client shared by every page.
 *
 * - Keeps the CSRF token in a module-level cache backed by localStorage so all
 *   tabs of the app share one token (sessionStorage broke multi-tab usage).
 * - `apiFetch` attaches `X-CSRF-Token` to unsafe methods automatically.
 * - On 401 or 403 `csrf_required` it recovers via GET /api/v1/auth/csrf
 *   (works when authenticated) and retries once; if recovery fails it
 *   redirects to /login as the canonical fallback.
 */

const CSRF_STORAGE_KEY = 'rivet.csrf';

let csrfToken: string | null = loadInitialCsrfToken();

function loadInitialCsrfToken(): string | null {
	if (typeof window === 'undefined') return null;
	try {
		// Migrate tokens written by older builds into sessionStorage only.
		return window.localStorage.getItem(CSRF_STORAGE_KEY) ?? window.sessionStorage.getItem('csrf');
	} catch {
		return null;
	}
}

/** Store the token issued by /api/v1/auth/login or the recovery endpoint. */
export function setCsrfToken(token: string): void {
	csrfToken = token;
	if (typeof window !== 'undefined') {
		try {
			window.localStorage.setItem(CSRF_STORAGE_KEY, token);
		} catch {
			// Storage unavailable (private mode etc.) — module cache still works.
		}
	}
}

export function clearCsrfToken(): void {
	csrfToken = null;
	if (typeof window !== 'undefined') {
		try {
			window.localStorage.removeItem(CSRF_STORAGE_KEY);
			window.sessionStorage.removeItem('csrf');
		} catch {
			// ignore
		}
	}
}

/** Headers carrying the current CSRF token, empty when none is cached. */
export function csrfHeader(): Record<string, string> {
	return csrfToken ? { 'x-csrf-token': csrfToken } : {};
}

async function refreshCsrfToken(): Promise<boolean> {
	// Only succeeds when we still have a valid session cookie.
	try {
		const res = await window.fetch('/api/v1/auth/csrf');
		if (!res.ok) return false;
		const data = (await res.json().catch(() => null)) as { token?: unknown } | null;
		if (!data || typeof data.token !== 'string') return false;
		setCsrfToken(data.token);
		return true;
	} catch {
		return false;
	}
}

async function send(input: RequestInfo | URL, init: RequestInit = {}): Promise<Response> {
	const method = (
		init.method ?? (input instanceof Request ? input.method : 'GET')
	).toUpperCase();
	const headers = new Headers(init.headers);
	if (method !== 'GET' && method !== 'HEAD' && method !== 'OPTIONS') {
		for (const [name, value] of Object.entries(csrfHeader())) {
			headers.set(name, value);
		}
	}
	return window.fetch(input, { ...init, headers });
}

/**
 * Read the machine-readable error code (`{"error": "..."}` envelope) of a
 * response without consuming its body.
 */
export async function errorCodeOf(res: Response): Promise<string | null> {
	const data = (await res.json().catch(() => null)) as { error?: unknown } | null;
	return data && typeof data.error === 'string' ? data.error : null;
}

/**
 * Fetch wrapper around window.fetch with CSRF attach + recovery semantics.
 * `allowRetry` guards against retry loops when called recursively.
 */
export async function apiFetch(
	input: RequestInfo | URL,
	init: RequestInit = {},
	allowRetry = true
): Promise<Response> {
	let res = await send(input, init);

	if (res.ok || !allowRetry) return res;

	if (res.status === 403) {
		// Only recover on the explicit CSRF rejection; other 403s are real denials.
		const code = await errorCodeOf(res.clone());
		if (code !== 'csrf_required') return res;
	} else if (res.status !== 401) {
		return res;
	}

	// Session may still be alive with a stale/missing token: re-mint and retry once.
	if (await refreshCsrfToken()) {
		res = await send(input, init);
		return res;
	}

	// No valid session — canonical fallback is the login page.
	window.location.assign('/login');
	return res;
}
