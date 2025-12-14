import * as server from '../entries/pages/setup/paths/_page.server.ts.js';

export const index = 6;
let component_cache;
export const component = async () => component_cache ??= (await import('../entries/pages/setup/paths/_page.svelte.js')).default;
export { server };
export const server_id = "src/routes/setup/paths/+page.server.ts";
export const imports = ["_app/immutable/nodes/6.tWLjO_o5.js","_app/immutable/chunks/suzeVJXb.js","_app/immutable/chunks/CMLA4Spu.js","_app/immutable/chunks/QtC0aoLg.js","_app/immutable/chunks/Bb0lR4-2.js","_app/immutable/chunks/B6JxemTf.js","_app/immutable/chunks/DT7fUnMD.js","_app/immutable/chunks/Cv8vTqDg.js"];
export const stylesheets = [];
export const fonts = [];
