import { redirect } from '@sveltejs/kit';

export async function load({ fetch }) {
	const res = await fetch('/api/v1/setup/status');
	if (res.ok) {
		const data = (await res.json()) as { setupMode: boolean };
		if (!data.setupMode) {
			throw redirect(302, '/');
		}
	}
	return {};
}
