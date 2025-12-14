import { a as attr } from "../../../../chunks/attributes.js";
import { c as pop, p as push } from "../../../../chunks/index.js";
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
export {
  _page as default
};
