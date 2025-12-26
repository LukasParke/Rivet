import { redirect } from '@sveltejs/kit';

export async function load({ fetch }) {
	const res = await fetch('/api/v1/setup/status');
	if (res.ok) {
		const data = (await res.json()) as { setupMode: boolean; dryRun?: boolean };
		// Allow access in dry run mode even if setup is complete
		if (!data.setupMode && !data.dryRun) {
			throw redirect(302, '/');
		}
	}
	return {};
}
