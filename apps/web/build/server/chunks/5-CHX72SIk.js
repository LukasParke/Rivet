import { r as redirect } from './index-wpIsICWW.js';

async function load({ fetch }) {
  const res = await fetch("/api/v1/setup/status");
  if (res.ok) {
    const data = await res.json();
    if (!data.setupMode) {
      throw redirect(302, "/");
    }
  }
  return {};
}

var _page_server_ts = /*#__PURE__*/Object.freeze({
  __proto__: null,
  load: load
});

const index = 5;
let component_cache;
const component = async () => component_cache ??= (await import('./_page.svelte-CX7npIms.js')).default;
const server_id = "src/routes/setup/+page.server.ts";
const imports = ["_app/immutable/nodes/5.CrgyP4C9.js","_app/immutable/chunks/CY-7o9oA.js","_app/immutable/chunks/Dc2GWTX7.js","_app/immutable/chunks/BLmVtnhA.js"];
const stylesheets = [];
const fonts = [];

export { component, fonts, imports, index, _page_server_ts as server, server_id, stylesheets };
//# sourceMappingURL=5-CHX72SIk.js.map
