import { r as redirect } from './index-wpIsICWW.js';

async function load({ fetch }) {
  const s = await fetch("/api/v1/setup/status");
  if (s.ok) {
    const data = await s.json();
    if (data.setupMode) throw redirect(302, "/setup");
  }
  const me = await fetch("/api/v1/auth/me");
  if (!me.ok) throw redirect(302, "/setup/admin");
  return {};
}

var _page_server_ts = /*#__PURE__*/Object.freeze({
  __proto__: null,
  load: load
});

const index = 4;
let component_cache;
const component = async () => component_cache ??= (await import('./_page.svelte-CqmSIdCJ.js')).default;
const server_id = "src/routes/app/+page.server.ts";
const imports = ["_app/immutable/nodes/4.KvuiqD-c.js","_app/immutable/chunks/CY-7o9oA.js","_app/immutable/chunks/Dc2GWTX7.js","_app/immutable/chunks/BLmVtnhA.js","_app/immutable/chunks/BOYLDhuR.js","_app/immutable/chunks/mBLmYUx9.js","_app/immutable/chunks/BTEFVq6Y.js","_app/immutable/chunks/DmQjgjFv.js"];
const stylesheets = [];
const fonts = [];

export { component, fonts, imports, index, _page_server_ts as server, server_id, stylesheets };
//# sourceMappingURL=4-DMWCrnRC.js.map
