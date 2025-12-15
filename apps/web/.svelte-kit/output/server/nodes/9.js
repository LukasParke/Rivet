import * as server from '../entries/pages/setup/indexers/_page.server.ts.js';

export const index = 9;
let component_cache;
export const component = async () => component_cache ??= (await import('../entries/pages/setup/indexers/_page.svelte.js')).default;
export { server };
export const server_id = "src/routes/setup/indexers/+page.server.ts";
export const imports = ["_app/immutable/nodes/9.BD1vkSr8.js","_app/immutable/chunks/CY-7o9oA.js","_app/immutable/chunks/Dc2GWTX7.js","_app/immutable/chunks/BLmVtnhA.js","_app/immutable/chunks/BOYLDhuR.js","_app/immutable/chunks/mBLmYUx9.js","_app/immutable/chunks/BTEFVq6Y.js","_app/immutable/chunks/DmQjgjFv.js"];
export const stylesheets = [];
export const fonts = [];
