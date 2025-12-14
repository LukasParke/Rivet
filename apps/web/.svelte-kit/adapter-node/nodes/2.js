import * as server from '../entries/pages/setup/_layout.server.ts.js';

export const index = 2;
let component_cache;
export const component = async () => component_cache ??= (await import('../entries/pages/setup/_layout.svelte.js')).default;
export { server };
export const server_id = "src/routes/setup/+layout.server.ts";
export const imports = ["_app/immutable/nodes/2.DU5eNPQi.js","_app/immutable/chunks/CY-7o9oA.js","_app/immutable/chunks/Dc2GWTX7.js","_app/immutable/chunks/BLmVtnhA.js","_app/immutable/chunks/DTnBG67W.js"];
export const stylesheets = [];
export const fonts = [];
