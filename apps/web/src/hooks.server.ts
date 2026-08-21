import { redirect, type Handle } from '@sveltejs/kit';

/**
 * Centralized routing guards.
 *
 * Every redirect decision lives here:
 *  - `/app/**` is protected: setup must be complete (or in setup mode the user
 *    is sent to the wizard) and a valid session is required. Unauthenticated
 *    users fall back to `/login` — the canonical login route for ALL protected
 *    pages (the review page previously bounced to /setup/admin).
 *  - `/login`: setup mode goes to the wizard; already-authenticated users go
 *    to /app.
 *  - `/setup/**`: only reachable while setupMode or dryRun is active.
 *  - `/`: routes to /setup, /app or /login depending on state.
 *
 * Per-page +page.server.ts files do data loads only.
 */

type SetupStatus = {
	setupMode?: boolean;
	setupComplete?: boolean;
	dryRun?: boolean;
	dryRunFixturePathSet?: boolean;
};
type FetchLike = (input: RequestInfo | URL, init?: RequestInit) => Promise<Response>;

async function fetchSetupStatus(fetchFn: FetchLike): Promise<SetupStatus | null> {
	try {
		const res = await fetchFn('/api/v1/setup/status');
		if (!res.ok) return null;
		return ((await res.json()) as SetupStatus) ?? null;
	} catch {
		return null;
	}
}

async function isAuthenticated(fetchFn: FetchLike): Promise<boolean> {
	try {
		const res = await fetchFn('/api/v1/auth/me');
		return res.ok;
	} catch {
		return false;
	}
}

export const handle: Handle = async ({ event, resolve }) => {
	const path = event.url.pathname;

	// Skip API calls, Kit internals and static assets.
	if (
		path.startsWith('/api') ||
		path.startsWith('/_app') ||
		path.startsWith('/favicon') ||
		path.includes('.')
	) {
		return resolve(event);
	}

	const status = await fetchSetupStatus(event.fetch);
	const setupMode = status?.setupMode === true;
	const setupComplete = status?.setupComplete === true;
	const dryRun = status?.dryRun === true;

	if (path === '/' || path === '') {
		if (setupMode || !setupComplete) throw redirect(302, '/setup');
		if (await isAuthenticated(event.fetch)) throw redirect(302, '/app');
		throw redirect(302, '/login');
	}

	if (path.startsWith('/setup')) {
		if (!setupMode && !dryRun) throw redirect(302, '/');
		return resolve(event);
	}

	if (path.startsWith('/login')) {
		if (setupMode) throw redirect(302, '/setup');
		if (await isAuthenticated(event.fetch)) throw redirect(302, '/app');
		return resolve(event);
	}

	if (path.startsWith('/app')) {
		if (setupMode || !setupComplete) throw redirect(302, '/setup');
		if (!(await isAuthenticated(event.fetch))) throw redirect(302, '/login');
		return resolve(event);
	}

	return resolve(event);
};
