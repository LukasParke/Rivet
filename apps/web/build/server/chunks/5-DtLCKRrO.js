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

const index = 5;
let component_cache;
const component = async () => component_cache ??= (await import('./_page.svelte-GnbrKWpA.js')).default;
const server_id = "src/routes/setup/admin/+page.server.ts";
const imports = ["_app/immutable/nodes/5.fTf4hllE.js","_app/immutable/chunks/suzeVJXb.js","_app/immutable/chunks/CMLA4Spu.js","_app/immutable/chunks/QtC0aoLg.js","_app/immutable/chunks/Bb0lR4-2.js","_app/immutable/chunks/B6JxemTf.js","_app/immutable/chunks/DT7fUnMD.js","_app/immutable/chunks/Cv8vTqDg.js"];
const stylesheets = [];
const fonts = [];

export { component, fonts, imports, index, _page_server_ts as server, server_id, stylesheets };
//# sourceMappingURL=5-DtLCKRrO.js.map
