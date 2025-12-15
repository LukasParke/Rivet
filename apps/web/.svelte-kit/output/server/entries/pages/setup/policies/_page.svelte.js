import { a as attr } from "../../../../chunks/attributes.js";
import { e as escape_html } from "../../../../chunks/escaping.js";
import { c as pop, p as push } from "../../../../chunks/index.js";
function _page($$payload, $$props) {
  push();
  let language = "en";
  let bannedTagsRaw = "cam\nts\nscr";
  let profileName = "Default";
  let allowedQualitiesRaw = "1080p\n2160p";
  let upgradeAllowed = true;
  let busy = false;
  $$payload.out += `<div class="card"><h2>Policies</h2> <p class="muted">This is a minimal v0 policy model to unblock the pipeline. We’ll expand profiles/rules later.</p> <div class="row"><input placeholder="Language (e.g. en)"${attr("value", language)}> <input placeholder="Profile name"${attr("value", profileName)}></div> <div style="margin-top: 12px;"><label class="muted" for="allowed_qualities">Allowed qualities (one per line)</label> <textarea id="allowed_qualities" rows="3" style="width: 100%; padding: 10px 12px; border: 1px solid #cbd5e1; border-radius: 10px; font-size: 14px;">`;
  const $$body = escape_html(allowedQualitiesRaw);
  if ($$body) {
    $$payload.out += `${$$body}`;
  }
  $$payload.out += `</textarea></div> <div style="margin-top: 12px;"><label class="muted" for="banned_tags">Banned tags (one per line)</label> <textarea id="banned_tags" rows="3" style="width: 100%; padding: 10px 12px; border: 1px solid #cbd5e1; border-radius: 10px; font-size: 14px;">`;
  const $$body_1 = escape_html(bannedTagsRaw);
  if ($$body_1) {
    $$payload.out += `${$$body_1}`;
  }
  $$payload.out += `</textarea></div> <div style="margin-top: 12px;"><label><input type="checkbox"${attr("checked", upgradeAllowed, true)}> Allow upgrades</label></div> <div class="row" style="margin-top: 12px;"><button${attr("disabled", busy, true)}>Validate</button> <button class="secondary"${attr("disabled", busy, true)}>Save &amp; Continue</button></div> `;
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
