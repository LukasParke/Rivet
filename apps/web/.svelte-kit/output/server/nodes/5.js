import * as server from '../entries/pages/setup/admin/_page.server.ts.js';

export const index = 5;
let component_cache;
export const component = async () => component_cache ??= (await import('../entries/pages/setup/admin/_page.svelte.js')).default;
export { server };
export const server_id = "src/routes/setup/admin/+page.server.ts";
export const imports = ["_app/immutable/nodes/5.fTf4hllE.js","_app/immutable/chunks/suzeVJXb.js","_app/immutable/chunks/CMLA4Spu.js","_app/immutable/chunks/QtC0aoLg.js","_app/immutable/chunks/Bb0lR4-2.js","_app/immutable/chunks/B6JxemTf.js","_app/immutable/chunks/DT7fUnMD.js","_app/immutable/chunks/Cv8vTqDg.js"];
export const stylesheets = [];
export const fonts = [];
