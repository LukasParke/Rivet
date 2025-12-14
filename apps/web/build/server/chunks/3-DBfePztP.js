import { r as redirect } from './index-wpIsICWW.js';

async function load({ fetch }) {
  const res = await fetch("/api/v1/setup/status");
  if (!res.ok) {
    return { setupMode: true };
  }
  const data = await res.json();
  if (data.setupMode) {
    throw redirect(302, "/setup");
  }
  return { setupMode: false };
}

var _page_server_ts = /*#__PURE__*/Object.freeze({
  __proto__: null,
  load: load
});

const index = 3;
let component_cache;
const component = async () => component_cache ??= (await import('./_page.svelte-Dvgy2JRq.js')).default;
const server_id = "src/routes/+page.server.ts";
const imports = ["_app/immutable/nodes/3.C-nGtdAx.js","_app/immutable/chunks/suzeVJXb.js","_app/immutable/chunks/CMLA4Spu.js","_app/immutable/chunks/QtC0aoLg.js"];
const stylesheets = [];
const fonts = [];

export { component, fonts, imports, index, _page_server_ts as server, server_id, stylesheets };
//# sourceMappingURL=3-DBfePztP.js.map
