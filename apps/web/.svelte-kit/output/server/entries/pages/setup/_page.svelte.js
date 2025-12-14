function _page($$payload) {
  $$payload.out += `<div class="card"><h2>Welcome</h2> <p class="muted">We’ll create an admin account, validate paths, configure download clients and indexers.</p> <div class="row"><a href="/setup/admin"><button>Create Admin</button></a> <a href="/setup/paths"><button class="secondary">Paths</button></a> <a href="/setup/clients"><button class="secondary">Download clients</button></a> <a href="/setup/indexers"><button class="secondary">Indexers</button></a></div></div>`;
}
export {
  _page as default
};
