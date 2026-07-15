const states = {
  idle: {
    template: "state-idle",
    caption: "空状态 · 环境身份与第一次输入",
  },
  working: {
    template: "state-working",
    caption: "执行中 · 主流程与工具活动",
  },
  permission: {
    template: "state-permission",
    caption: "权限确认 · 高风险操作独立决策",
  },
  complete: {
    template: "state-complete",
    caption: "完成态 · 结果、变更与验证证据",
  },
};

const screen = document.querySelector("#prototype-screen");
const caption = document.querySelector("#state-caption");
const buttons = [...document.querySelectorAll("[data-state]")];

function requestedState() {
  const query = new URLSearchParams(window.location.search).get("state");
  const hash = window.location.hash.slice(1);
  if (states[query]) return query;
  if (states[hash]) return hash;
  return "working";
}

function bindToolRows() {
  screen.querySelectorAll("[data-tool] .tool-summary").forEach((button) => {
    button.addEventListener("click", () => {
      const detail = button.nextElementSibling;
      const expanded = button.getAttribute("aria-expanded") === "true";
      button.setAttribute("aria-expanded", String(!expanded));
      detail.hidden = expanded;
    });
  });
}

function showState(name, updateLocation = true) {
  const state = states[name] ?? states.working;
  const template = document.querySelector(`#${state.template}`);
  screen.replaceChildren(template.content.cloneNode(true));
  caption.textContent = state.caption;
  document.documentElement.dataset.prototypeState = name;
  buttons.forEach((button) => {
    const active = button.dataset.state === name;
    button.classList.toggle("is-active", active);
    button.setAttribute("aria-pressed", String(active));
  });
  bindToolRows();
  if (updateLocation && window.location.hash !== `#${name}`) {
    history.replaceState(null, "", `#${name}`);
  }
}

buttons.forEach((button) => {
  button.addEventListener("click", () => showState(button.dataset.state));
});

document.addEventListener("keydown", (event) => {
  const names = ["idle", "working", "permission", "complete"];
  const index = Number(event.key) - 1;
  if (Number.isInteger(index) && names[index]) {
    showState(names[index]);
  }
});

window.addEventListener("hashchange", () => showState(requestedState(), false));
showState(requestedState(), false);
