import * as server from '../entries/pages/setup/_layout.server.ts.js';

export const index = 2;
let component_cache;
export const component = async () => component_cache ??= (await import('../entries/pages/setup/_layout.svelte.js')).default;
export { server };
export const server_id = "src/routes/setup/+layout.server.ts";
export const imports = ["_app/immutable/nodes/2.ObO1BwDD.js","_app/immutable/chunks/suzeVJXb.js","_app/immutable/chunks/CMLA4Spu.js","_app/immutable/chunks/QtC0aoLg.js","_app/immutable/chunks/DFaotDud.js"];
export const stylesheets = [];
export const fonts = [];
