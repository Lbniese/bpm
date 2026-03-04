const fs = require("node:fs");
const path = require("node:path");
const seedrandom = require("seedrandom");

const outputRoot = path.join(__dirname, "..", "generated");
const directoryCount = 32;
const filesPerDirectory = 32;

function expectedFilePath(index) {
  const directory = Math.floor(index / filesPerDirectory);
  return path.join(outputRoot, `group-${directory.toString().padStart(2, "0")}`, `file-${index.toString().padStart(4, "0")}.txt`);
}

function generate() {
  fs.rmSync(outputRoot, { force: true, recursive: true });
  const random = seedrandom("bpm-many-small-files-v1");

  for (let index = 0; index < directoryCount * filesPerDirectory; index += 1) {
    const file = expectedFilePath(index);
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, `${index}:${random().toFixed(16)}\n`);
  }

  process.stdout.write(`${directoryCount * filesPerDirectory} files generated\n`);
}

function check() {
  for (let index = 0; index < directoryCount * filesPerDirectory; index += 1) {
    if (!fs.statSync(expectedFilePath(index)).isFile()) {
      throw new Error(`missing generated fixture file ${index}`);
    }
  }

  process.stdout.write("many-small-files:ok\n");
}

if (process.argv.includes("--check")) {
  check();
} else {
  generate();
}
