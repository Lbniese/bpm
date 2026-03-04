const fs = require("node:fs");
const path = require("node:path");

const output = path.join(__dirname, "..", "lifecycle-output.txt");
const phases = ["preinstall", "install", "postinstall"];
const phase = process.argv[2];

if (phase === "--check") {
  const actual = fs.readFileSync(output, "utf8");
  const expected = `${phases.join("\n")}\n`;
  if (actual !== expected) {
    throw new Error(`unexpected lifecycle output: ${JSON.stringify(actual)}`);
  }
  process.stdout.write("lifecycle:ok\n");
} else {
  if (!phases.includes(phase)) {
    throw new Error(`unsupported lifecycle phase: ${phase}`);
  }
  const flag = phase === phases[0] ? "w" : "a";
  fs.writeFileSync(output, `${phase}\n`, { flag });
}
