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

var _layout_server_ts = /*#__PURE__*/Object.freeze({
  __proto__: null,
  load: load
});

const index = 2;
let component_cache;
const component = async () => component_cache ??= (await import('./_layout.svelte-BvsjLV7s.js')).default;
const server_id = "src/routes/setup/+layout.server.ts";
const imports = ["_app/immutable/nodes/2.ObO1BwDD.js","_app/immutable/chunks/suzeVJXb.js","_app/immutable/chunks/CMLA4Spu.js","_app/immutable/chunks/QtC0aoLg.js","_app/immutable/chunks/DFaotDud.js"];
const stylesheets = [];
const fonts = [];

export { component, fonts, imports, index, _layout_server_ts as server, server_id, stylesheets };
//# sourceMappingURL=2-VybbhsTw.js.map
