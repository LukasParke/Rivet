import * as server from '../entries/pages/setup/_layout.server.ts.js';

export const index = 2;
let component_cache;
export const component = async () => component_cache ??= (await import('../entries/pages/setup/_layout.svelte.js')).default;
export { server };
export const server_id = "src/routes/setup/+layout.server.ts";
export const imports = ["_app/immutable/nodes/2.BX7dRkyY.js","_app/immutable/chunks/9z75sjEA.js","_app/immutable/chunks/D65C3x6o.js","_app/immutable/chunks/RwSj8A9l.js","_app/immutable/chunks/wWFkWsRQ.js"];
export const stylesheets = [];
export const fonts = [];
