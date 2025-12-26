import { redirect } from '@sveltejs/kit';

export async function load({ fetch }) {
	const res = await fetch('/api/v1/setup/status');
	if (!res.ok) {
		return { setupMode: true };
	}
	const data = (await res.json()) as { setupMode: boolean; setupComplete?: boolean };
	if (data.setupMode) {
		throw redirect(302, '/setup');
	}
	// Check if user is authenticated
	const me = await fetch('/api/v1/auth/me');
	if (me.ok) {
		// User is authenticated
		// If setup is not complete, redirect to setup wizard
		if (!data.setupComplete) {
			throw redirect(302, '/setup');
		}
		// Setup is complete, go to app
		throw redirect(302, '/app');
	}
	// User is not authenticated, go to login
	throw redirect(302, '/login');
}
