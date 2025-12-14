import { a as attr } from './attributes-BeaNKpgU.js';
import { p as push, c as pop } from './index-DyZnF-0d.js';
import './escaping-CqgfEcN3.js';

function _page($$payload, $$props) {
  push();
  let busy = false;
  $$payload.out += `<div class="card"><h2>Finish</h2> <p class="muted">This records a setup report snapshot and closes registration by default.</p> <div class="row"><button${attr("disabled", busy, true)}>Finish setup</button> <a href="/"><button class="secondary">Go to app</button></a></div> `;
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
//# sourceMappingURL=_page.svelte-BOuz5pNv.js.map
