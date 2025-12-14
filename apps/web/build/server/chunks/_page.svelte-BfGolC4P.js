import { a as attr } from './attributes-BeaNKpgU.js';
import { p as push, c as pop } from './index-DyZnF-0d.js';
import './escaping-CqgfEcN3.js';

function _page($$payload, $$props) {
  push();
  let kind = "movie";
  let externalSource = "tmdb";
  let externalId = "550";
  let query = "Fight Club 1999 1080p";
  let busy = false;
  $$payload.out += `<div class="container"><h1>Rivet (MVP pipeline demo)</h1> <p class="muted">Create a target, run an indexer search, store candidates + decisions.</p> <div class="card"><h2>Create target</h2> <div class="row"><input placeholder="kind (movie/episode/season_pack)"${attr("value", kind)}> <input placeholder="externalSource (tmdb/tvdb)"${attr("value", externalSource)}> <input placeholder="externalId"${attr("value", externalId)}> <input placeholder="query"${attr("value", query)}> <button${attr("disabled", busy, true)}>Create</button></div> `;
  {
    $$payload.out += "<!--[!-->";
  }
  $$payload.out += `<!--]--> `;
  {
    $$payload.out += "<!--[!-->";
  }
  $$payload.out += `<!--]--> `;
  {
    $$payload.out += "<!--[!-->";
  }
  $$payload.out += `<!--]--></div> `;
  {
    $$payload.out += "<!--[!-->";
  }
  $$payload.out += `<!--]--></div>`;
  pop();
}

export { _page as default };
//# sourceMappingURL=_page.svelte-BfGolC4P.js.map
