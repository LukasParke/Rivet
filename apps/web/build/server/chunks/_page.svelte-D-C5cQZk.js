import { a as attr } from './attributes-BeaNKpgU.js';
import { e as escape_html } from './escaping-CqgfEcN3.js';
import { p as push, c as pop } from './index-DyZnF-0d.js';

function _page($$payload, $$props) {
  push();
  let downloadsCompletedPath = "/downloads/completed";
  let quarantinePath = "/quarantine";
  let libraryRootsRaw = "/library";
  let busy = false;
  $$payload.out += `<div class="card"><h2>Paths</h2> <p class="muted">These paths must be accessible to the container. Hardlink viability is checked between downloads and library roots.</p> <div class="row"><input placeholder="Downloads completed path"${attr("value", downloadsCompletedPath)}> <input placeholder="Quarantine path"${attr("value", quarantinePath)}></div> <div style="margin-top: 12px;"><textarea rows="4" style="width: 100%; padding: 10px 12px; border: 1px solid #cbd5e1; border-radius: 10px; font-size: 14px;" placeholder="Library roots (one per line)">`;
  const $$body = escape_html(libraryRootsRaw);
  if ($$body) {
    $$payload.out += `${$$body}`;
  }
  $$payload.out += `</textarea></div> <div class="row" style="margin-top: 12px;"><button${attr("disabled", busy, true)}>Validate</button> <button class="secondary"${attr("disabled", true, true)}>Save</button></div> `;
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
//# sourceMappingURL=_page.svelte-D-C5cQZk.js.map
