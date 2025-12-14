import { a as attr } from './attributes-BeaNKpgU.js';
import { p as push, c as pop } from './index-DyZnF-0d.js';
import './escaping-CqgfEcN3.js';

function _page($$payload, $$props) {
  push();
  let torznabUrl = "";
  let torznabKey = "";
  let torznabName = "Torznab";
  let newznabUrl = "";
  let newznabKey = "";
  let newznabName = "Newznab";
  let includeTorznab = true;
  let includeNewznab = true;
  let busy = false;
  $$payload.out += `<div class="card"><h2>Indexers</h2> <p class="muted">We’ll run a sample search and parse the RSS response.</p> <h3>Torznab</h3> <label><input type="checkbox"${attr("checked", includeTorznab, true)}> Enable</label> <div class="row" style="margin-top: 8px;"><input placeholder="Name"${attr("value", torznabName)}${attr("disabled", !includeTorznab, true)}> <input placeholder="Base URL (e.g. https://indexer)"${attr("value", torznabUrl)}${attr("disabled", !includeTorznab, true)}> <input placeholder="API key" type="password"${attr("value", torznabKey)}${attr("disabled", !includeTorznab, true)}></div> <h3 style="margin-top: 16px;">Newznab</h3> <label><input type="checkbox"${attr("checked", includeNewznab, true)}> Enable</label> <div class="row" style="margin-top: 8px;"><input placeholder="Name"${attr("value", newznabName)}${attr("disabled", !includeNewznab, true)}> <input placeholder="Base URL (e.g. https://indexer)"${attr("value", newznabUrl)}${attr("disabled", !includeNewznab, true)}> <input placeholder="API key" type="password"${attr("value", newznabKey)}${attr("disabled", !includeNewznab, true)}></div> <div class="row" style="margin-top: 12px;"><button${attr("disabled", busy, true)}>Validate</button> <button class="secondary"${attr("disabled", true, true)}>Save</button></div> `;
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
//# sourceMappingURL=_page.svelte-CFqCSNhP.js.map
