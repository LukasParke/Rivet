import { r as redirect } from './index-wpIsICWW.js';

async function load({ fetch }) {
  const s = await fetch("/api/v1/setup/status");
  if (s.ok) {
    const data = await s.json();
    if (!data.setupMode) throw redirect(302, "/");
  }
  const me = await fetch("/api/v1/auth/me");
  if (!me.ok) throw redirect(302, "/setup/admin");
  return {};
}

var _page_server_ts = /*#__PURE__*/Object.freeze({
  __proto__: null,
  load: load
});

const index = 11;
let component_cache;
const component = async () => component_cache ??= (await import('./_page.svelte-CyINLnMU.js')).default;
const server_id = "src/routes/setup/policies/+page.server.ts";
const imports = ["_app/immutable/nodes/11.C4P5nu6G.js","_app/immutable/chunks/9z75sjEA.js","_app/immutable/chunks/D65C3x6o.js","_app/immutable/chunks/RwSj8A9l.js","_app/immutable/chunks/DmVZp85x.js","_app/immutable/chunks/D5Af684K.js","_app/immutable/chunks/Bqa4hpEt.js","_app/immutable/chunks/dc82Tup1.js"];
const stylesheets = [];
const fonts = [];

export { component, fonts, imports, index, _page_server_ts as server, server_id, stylesheets };
//# sourceMappingURL=11-DDRWC513.js.map
