const LATTE_PIXELS = [
  "         TT",
  "         TT",
  "         TTT",
  "        TTTT",
  "       TTTT",
  "      TTTT",
  "      TTT",
  "      TT",
  "      TT",
  "      TT",
  "",
  " DDDDDDDDDDDDDDD",
  "DDDDDDDDDDDDDDDDD",
  "DDD           DDDDD",
  "DDDSSSSSSSSSS DDDDD",
  "DDDSSSSSSSSSS DD DD",
  "DDDSSSSSSSSSS DD DD",
  " DDSSSSSSSSSS DDDDD",
  " DD SSSSSSSS DDDDD",
  " DDD SSSSSS DDD",
  "  DDDDDDDDDDDD",
  "   DDDDDDDDDD",
  "      DDDD",
  "",
];

const PIXEL_WIDTH = 20;
const paletteName = {
  " ": "empty",
  D: "outline",
  S: "sage",
  T: "steam",
};

function renderPixelLogo(target) {
  const rows = LATTE_PIXELS.map((row) => row.padEnd(PIXEL_WIDTH, " ").slice(0, PIXEL_WIDTH));
  const fragment = document.createDocumentFragment();

  for (let y = 0; y < rows.length; y += 2) {
    const line = document.createElement("div");
    line.className = "pixel-row";

    for (let x = 0; x < PIXEL_WIDTH; x += 1) {
      const top = rows[y][x];
      const bottom = rows[y + 1]?.[x] ?? " ";
      const cell = document.createElement("span");
      cell.className = "pixel-cell";
      cell.textContent = "▀";
      cell.style.color = `var(--px-${paletteName[top]})`;
      cell.style.backgroundColor = `var(--px-${paletteName[bottom]})`;
      line.append(cell);
    }

    fragment.append(line);
  }

  target.replaceChildren(fragment);
}

document.querySelectorAll("[data-pixel-logo]").forEach(renderPixelLogo);
