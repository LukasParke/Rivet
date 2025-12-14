import type { Handle } from '@sveltejs/kit';

// For now we assume API and web are served from the same origin via reverse proxy.
// This hook is a placeholder for future auth/session handling.
export const handle: Handle = async ({ event, resolve }) => {
	return resolve(event);
};
