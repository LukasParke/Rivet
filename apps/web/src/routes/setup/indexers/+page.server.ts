import { redirect } from '@sveltejs/kit';

export async function load({ fetch }) {
	const s = await fetch('/api/v1/setup/status');
	if (s.ok) {
		const data = (await s.json()) as { setupMode: boolean };
		if (!data.setupMode) throw redirect(302, '/');
	}

	const me = await fetch('/api/v1/auth/me');
	if (!me.ok) throw redirect(302, '/setup/admin');

	return {};
}
