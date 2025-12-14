import * as server from '../entries/pages/setup/policies/_page.server.ts.js';

export const index = 11;
let component_cache;
export const component = async () => component_cache ??= (await import('../entries/pages/setup/policies/_page.svelte.js')).default;
export { server };
export const server_id = "src/routes/setup/policies/+page.server.ts";
export const imports = ["_app/immutable/nodes/11.C4P5nu6G.js","_app/immutable/chunks/9z75sjEA.js","_app/immutable/chunks/D65C3x6o.js","_app/immutable/chunks/RwSj8A9l.js","_app/immutable/chunks/DmVZp85x.js","_app/immutable/chunks/D5Af684K.js","_app/immutable/chunks/Bqa4hpEt.js","_app/immutable/chunks/dc82Tup1.js"];
export const stylesheets = [];
export const fonts = [];
