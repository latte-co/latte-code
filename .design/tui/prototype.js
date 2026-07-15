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
  todo: {
    template: "state-todo",
    caption: "Todo · 固定任务清单与状态变化",
  },
  subagent: {
    template: "state-subagent",
    caption: "Subagents · Main 与临时后台任务的父子调用流",
  },
  team: {
    template: "state-agent-team",
    caption: "Agent Team · Lead 主会话、稳定成员与持续 steering",
  },
  agents: {
    template: "state-agent-sessions",
    caption: "Agents · 后台运行实例的完整公开会话",
  },
};

const screen = document.querySelector("#prototype-screen");
const caption = document.querySelector("#state-caption");
const buttons = [...document.querySelectorAll("[data-state]")];
let selectedAgent = "main";
let selectedAgentView = "sessions";
let inspectorContext = "lineage";

function requestedState() {
  const query = new URLSearchParams(window.location.search).get("state");
  const hash = window.location.hash.slice(1);
  if (states[query]) return query;
  if (states[hash]) return hash;
  return "working";
}

function bindDisclosureRows() {
  screen.querySelectorAll("[data-tool] .tool-summary, [data-phase] .phase-summary, [data-todo] .todo-summary, [data-agents] .agents-summary").forEach((button) => {
    button.addEventListener("click", () => {
      const detail = button.nextElementSibling;
      const expanded = button.getAttribute("aria-expanded") === "true";
      button.setAttribute("aria-expanded", String(!expanded));
      detail.hidden = expanded;
    });
  });
}

function selectAgentSession(name) {
  const available = [...screen.querySelectorAll("[data-agent-session]")];
  if (!available.some((button) => button.dataset.agentSession === name)) {
    name = available[0]?.dataset.agentSession ?? name;
  }
  selectedAgent = name;
  available.forEach((button) => {
    button.classList.toggle("is-selected", button.dataset.agentSession === name);
  });
  screen.querySelectorAll("[data-session-panel]").forEach((panel) => {
    panel.hidden = panel.dataset.sessionPanel !== name;
  });
}

function selectAgentView(name) {
  selectedAgentView = name;
  screen.querySelectorAll("[data-agent-view]").forEach((button) => {
    button.classList.toggle("is-active", button.dataset.agentView === name);
  });
  screen.querySelectorAll("[data-agent-view-panel]").forEach((panel) => {
    panel.hidden = panel.dataset.agentViewPanel !== name;
  });
}

function bindAgentTree() {
  screen.querySelectorAll("[data-agent-tree-toggle]").forEach((toggle) => {
    toggle.addEventListener("click", (event) => {
      event.stopPropagation();
      const branch = toggle.closest("[data-agent-branch]");
      const children = branch?.querySelector(".agent-tree-children");
      if (!children) return;
      const expanded = toggle.getAttribute("aria-expanded") === "true";
      toggle.setAttribute("aria-expanded", String(!expanded));
      toggle.setAttribute("aria-label", expanded ? "Expand Explore children" : "Collapse Explore children");
      children.hidden = expanded;
    });
  });
}

function bindAgentViews() {
  screen.querySelectorAll("[data-agent-view]").forEach((button) => {
    button.addEventListener("click", () => selectAgentView(button.dataset.agentView));
  });
  if (screen.querySelector("[data-agent-view-panel]")) {
    selectAgentView(selectedAgentView);
  }
}

function bindComposerTarget() {
  const button = screen.querySelector("[data-target-cycle]");
  const placeholder = screen.querySelector("[data-target-placeholder]");
  if (!button || !placeholder) return;

  const targets = ["Lead", "Explore", "Test", "Reviewer"];
  let targetIndex = 0;
  button.addEventListener("click", () => {
    targetIndex = (targetIndex + 1) % targets.length;
    const target = targets[targetIndex];
    const chevron = document.createElement("span");
    chevron.textContent = "▾";
    button.replaceChildren(document.createTextNode(`${target} `), chevron);
    placeholder.textContent = target === "Lead"
      ? "Message Lead while the team works…"
      : `Send a visible message to ${target}…`;
  });
}

function bindAgentNavigation() {
  screen.querySelectorAll("[data-open-agent]").forEach((button) => {
    button.addEventListener("click", () => {
      selectedAgent = button.dataset.openAgent;
      selectedAgentView = "sessions";
      inspectorContext = button.dataset.agentContext ?? "lineage";
      showState("agents");
    });
  });
  screen.querySelectorAll("[data-agent-session]").forEach((button) => {
    button.addEventListener("click", () => selectAgentSession(button.dataset.agentSession));
  });
  if (screen.querySelector("[data-session-panel]")) {
    selectAgentSession(selectedAgent);
  }
}

function showState(name, updateLocation = true) {
  const state = states[name] ?? states.working;
  const templateName = name === "agents" && inspectorContext === "team"
    ? "state-team-agent-sessions"
    : state.template;
  const template = document.querySelector(`#${templateName}`);
  screen.replaceChildren(template.content.cloneNode(true));
  caption.textContent = name === "agents"
    ? (inspectorContext === "team"
      ? "Agents · Team 运行树、共享任务与实时活动"
      : "Agents · Main Session 的后台调用树与完整公开会话")
    : state.caption;
  document.documentElement.dataset.prototypeState = name;
  buttons.forEach((button) => {
    const active = button.dataset.state === name;
    button.classList.toggle("is-active", active);
    button.setAttribute("aria-pressed", String(active));
  });
  bindDisclosureRows();
  bindAgentNavigation();
  bindAgentTree();
  bindAgentViews();
  bindComposerTarget();
  if (updateLocation && window.location.hash !== `#${name}`) {
    history.replaceState(null, "", `#${name}`);
  }
}

buttons.forEach((button) => {
  button.addEventListener("click", () => {
    if (button.dataset.state === "agents") {
      const current = document.documentElement.dataset.prototypeState;
      if (current === "team") inspectorContext = "team";
      if (current === "subagent") inspectorContext = "lineage";
    }
    showState(button.dataset.state);
  });
});

document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && document.documentElement.dataset.prototypeState === "agents") {
    showState(inspectorContext === "team" ? "team" : "subagent");
    return;
  }
  const names = ["idle", "working", "permission", "complete", "todo", "subagent", "team", "agents"];
  const index = Number(event.key) - 1;
  if (Number.isInteger(index) && names[index]) {
    if (names[index] === "agents") {
      const current = document.documentElement.dataset.prototypeState;
      if (current === "team") inspectorContext = "team";
      if (current === "subagent") inspectorContext = "lineage";
    }
    showState(names[index]);
  }
});

window.addEventListener("hashchange", () => showState(requestedState(), false));
showState(requestedState(), false);
