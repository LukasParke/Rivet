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
const component = async () => component_cache ??= (await import('./_page.svelte-DYrk4GVs.js')).default;
const server_id = "src/routes/setup/+page.server.ts";
const imports = ["_app/immutable/nodes/4.Dd1uHLB1.js","_app/immutable/chunks/9z75sjEA.js","_app/immutable/chunks/D65C3x6o.js","_app/immutable/chunks/RwSj8A9l.js"];
const stylesheets = [];
const fonts = [];

export { component, fonts, imports, index, _page_server_ts as server, server_id, stylesheets };
//# sourceMappingURL=4-BSCSu-io.js.map
