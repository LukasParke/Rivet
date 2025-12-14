import { redirect } from '@sveltejs/kit';

export async function load({ fetch }) {
	const res = await fetch('/api/v1/setup/status');
	if (!res.ok) {
		// If API isn't reachable yet, still show setup entry.
		return { setupMode: true };
	}
	const data = (await res.json()) as { setupMode: boolean };
	if (data.setupMode) {
		throw redirect(302, '/setup');
	}
	// Placeholder until the full app UI exists.
	return { setupMode: false };
}
