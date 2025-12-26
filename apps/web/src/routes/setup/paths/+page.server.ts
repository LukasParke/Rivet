import { redirect } from '@sveltejs/kit';

export async function load({ fetch }) {
	// Require setup mode (or dry run mode).
	const s = await fetch('/api/v1/setup/status');
	if (s.ok) {
		const data = (await s.json()) as { setupMode: boolean; dryRun?: boolean };
		// Allow access in dry run mode even if setup is complete
		if (!data.setupMode && !data.dryRun) throw redirect(302, '/');
	}
	// Require auth (admin) to run validations.
	const me = await fetch('/api/v1/auth/me');
	if (!me.ok) {
		throw redirect(302, '/setup/admin');
	}
	return {};
}
