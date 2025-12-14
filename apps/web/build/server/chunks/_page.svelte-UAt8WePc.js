import { a as attr } from './attributes-BeaNKpgU.js';
import { p as push, c as pop } from './index-DyZnF-0d.js';
import './escaping-CqgfEcN3.js';

function _page($$payload, $$props) {
  push();
  let tmdbBaseUrl = "https://api.themoviedb.org";
  let tmdbKey = "";
  let includeTMDB = true;
  let tvdbBaseUrl = "https://api4.thetvdb.com";
  let tvdbKey = "";
  let includeTVDB = false;
  let busy = false;
  $$payload.out += `<div class="card"><h2>Metadata providers</h2> <p class="muted">We’ll validate your API keys and set request budgets in a later step.</p> <h3>TMDB</h3> <label><input type="checkbox"${attr("checked", includeTMDB, true)}> Enable</label> <div class="row" style="margin-top: 8px;"><input placeholder="Base URL"${attr("value", tmdbBaseUrl)}${attr("disabled", !includeTMDB, true)}> <input placeholder="API key" type="password"${attr("value", tmdbKey)}${attr("disabled", !includeTMDB, true)}></div> <h3 style="margin-top: 16px;">TVDB</h3> <label><input type="checkbox"${attr("checked", includeTVDB, true)}> Enable</label> <div class="row" style="margin-top: 8px;"><input placeholder="Base URL"${attr("value", tvdbBaseUrl)}${attr("disabled", !includeTVDB, true)}> <input placeholder="API key" type="password"${attr("value", tvdbKey)}${attr("disabled", !includeTVDB, true)}></div> <div class="row" style="margin-top: 12px;"><button${attr("disabled", busy, true)}>Validate</button> <button class="secondary"${attr("disabled", true, true)}>Save</button></div> `;
  {
    $$payload.out += "<!--[!-->";
  }
  $$payload.out += `<!--]--> `;
  {
    $$payload.out += "<!--[!-->";
  }
  $$payload.out += `<!--]--></div>`;
  pop();
}

export { _page as default };
//# sourceMappingURL=_page.svelte-UAt8WePc.js.map
