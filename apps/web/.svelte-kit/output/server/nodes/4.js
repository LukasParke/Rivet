import * as server from '../entries/pages/setup/_page.server.ts.js';

export const index = 4;
let component_cache;
export const component = async () => component_cache ??= (await import('../entries/pages/setup/_page.svelte.js')).default;
export { server };
export const server_id = "src/routes/setup/+page.server.ts";
export const imports = ["_app/immutable/nodes/4.B6-CEi-4.js","_app/immutable/chunks/suzeVJXb.js","_app/immutable/chunks/CMLA4Spu.js","_app/immutable/chunks/QtC0aoLg.js"];
export const stylesheets = [];
export const fonts = [];
