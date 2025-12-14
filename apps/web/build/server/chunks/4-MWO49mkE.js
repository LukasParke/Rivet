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

const index = 4;
let component_cache;
const component = async () => component_cache ??= (await import('./_page.svelte-BFVdODhD.js')).default;
const server_id = "src/routes/setup/+page.server.ts";
const imports = ["_app/immutable/nodes/4.B6-CEi-4.js","_app/immutable/chunks/suzeVJXb.js","_app/immutable/chunks/CMLA4Spu.js","_app/immutable/chunks/QtC0aoLg.js"];
const stylesheets = [];
const fonts = [];

export { component, fonts, imports, index, _page_server_ts as server, server_id, stylesheets };
//# sourceMappingURL=4-MWO49mkE.js.map
