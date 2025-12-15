import * as server from '../entries/pages/setup/_page.server.ts.js';

export const index = 5;
let component_cache;
export const component = async () => component_cache ??= (await import('../entries/pages/setup/_page.svelte.js')).default;
export { server };
export const server_id = "src/routes/setup/+page.server.ts";
export const imports = ["_app/immutable/nodes/5.CrgyP4C9.js","_app/immutable/chunks/CY-7o9oA.js","_app/immutable/chunks/Dc2GWTX7.js","_app/immutable/chunks/BLmVtnhA.js"];
export const stylesheets = [];
export const fonts = [];
