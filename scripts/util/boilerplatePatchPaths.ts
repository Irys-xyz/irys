import { promises } from "fs";
import { readFile } from "fs/promises";
import { dirname, join, relative } from "path";

async function* walk(dir: string): AsyncGenerator<string, void, any> {
  for await (const d of await promises.opendir(dir)) {
    const entry = join(dir, d.name);
    if (d.isDirectory()) yield* await walk(entry);
    else if (d.isFile() && d.name === "Cargo.toml") yield entry;
  }
}

(async function () {
  const overrides = [];
  const base = "../../";
  for await (const toml of walk("../../ext/alloy-core/crates")) {
    console.log(toml);
    const contents = (await readFile(toml)).toString();
    const nameLine = contents
      .split("\n")
      .find((l) => l.trim().startsWith('name = "'));
    if (!nameLine) {
      throw new Error(`No name found in ${toml}`);
    }
    const name = nameLine.match(/name\s*=\s*"([^"]+)"/)?.[1];
    if (!name) {
      throw new Error(`Invalid name format in ${toml}`);
    }
    console.log(contents, name);
    const overrideString = `${name} = { path = "./${relative(base, dirname(toml))}" }`;
    overrides.push(overrideString);
  }
  console.log(overrides.join("\n"));
})();
