import { redirect } from '@sveltejs/kit';

export async function load({ fetch }) {
	const s = await fetch('/api/v1/setup/status');
	if (s.ok) {
		const data = (await s.json()) as { setupMode: boolean; setupComplete?: boolean };
		if (data.setupMode) throw redirect(302, '/setup');
		// If setup is not complete, redirect to setup wizard
		if (!data.setupComplete) {
			throw redirect(302, '/setup');
		}
	}
	const me = await fetch('/api/v1/auth/me');
	if (!me.ok) {
		// If setup is complete, redirect to login page instead of setup/admin
		throw redirect(302, '/login');
	}
	return {};
}
