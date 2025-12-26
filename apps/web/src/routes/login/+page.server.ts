import { redirect } from '@sveltejs/kit';

export async function load({ fetch }) {
	// Check if setup is complete
	const res = await fetch('/api/v1/setup/status');
	if (res.ok) {
		const data = (await res.json()) as { setupMode: boolean };
		// If setup is not complete, redirect to setup
		if (data.setupMode) {
			throw redirect(302, '/setup');
		}
	}

	// If user is already authenticated, redirect to app
	const me = await fetch('/api/v1/auth/me');
	if (me.ok) {
		throw redirect(302, '/app');
	}

	// Otherwise, allow access to login page
	return {};
}

