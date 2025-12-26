import { redirect } from '@sveltejs/kit';

export async function load({ fetch }) {
	const s = await fetch('/api/v1/setup/status');
	if (s.ok) {
		const data = (await s.json()) as { setupMode: boolean; dryRun?: boolean };
		// Allow access in dry run mode even if setup is complete
		if (!data.setupMode && !data.dryRun) throw redirect(302, '/');
	}
	const me = await fetch('/api/v1/auth/me');
	if (!me.ok) throw redirect(302, '/setup/admin');
	return {};
}
