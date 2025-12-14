import { r as redirect } from './index-wpIsICWW.js';

async function load({ fetch }) {
  const s = await fetch("/api/v1/setup/status");
  if (s.ok) {
    const data = await s.json();
    if (!data.setupMode) throw redirect(302, "/");
  }
  return {};
}

var _page_server_ts = /*#__PURE__*/Object.freeze({
  __proto__: null,
  load: load
});

const index = 6;
let component_cache;
const component = async () => component_cache ??= (await import('./_page.svelte-GnbrKWpA.js')).default;
const server_id = "src/routes/setup/admin/+page.server.ts";
const imports = ["_app/immutable/nodes/6.E6dmELFw.js","_app/immutable/chunks/CY-7o9oA.js","_app/immutable/chunks/Dc2GWTX7.js","_app/immutable/chunks/BLmVtnhA.js","_app/immutable/chunks/BOYLDhuR.js","_app/immutable/chunks/mBLmYUx9.js","_app/immutable/chunks/BTEFVq6Y.js","_app/immutable/chunks/DmQjgjFv.js"];
const stylesheets = [];
const fonts = [];

export { component, fonts, imports, index, _page_server_ts as server, server_id, stylesheets };
//# sourceMappingURL=6-LW8e28t_.js.map
