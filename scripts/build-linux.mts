#!/usr/bin/env node

import { createHash } from "node:crypto";
import { createReadStream } from "node:fs";
import { access, chmod, copyFile, mkdir, writeFile } from "node:fs/promises";
import { constants } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { Command } from "commander";

type Arch = "arm64" | "x64";

type Options = {
  arch: Arch;
  archive: boolean;
  bin: string;
  cargoArgs: string[];
  glibc: string;
  locked: boolean;
  packageName: string;
  profile: string;
  target: string;
};

const archTargets: Record<Arch, string> = {
  arm64: "aarch64-unknown-linux-gnu",
  x64: "x86_64-unknown-linux-gnu",
};

const scriptDir = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(scriptDir, "..");

const defaultOptions: Omit<Options, "arch" | "target"> = {
  archive: true,
  bin: "coda-server",
  cargoArgs: [],
  glibc: "2.31",
  locked: true,
  packageName: "coda_server",
  profile: "release",
};

function parseArgs(argv: string[]): Options {
  const program = new Command()
    .name("build-linux")
    .description("Build coda-server for Linux (arm64 or x86_64) with cargo-zigbuild.")
    .requiredOption("--arch <arch>", "target architecture: arm64 or x64")
    .option("--glibc <version>", "glibc baseline for cargo-zigbuild", defaultOptions.glibc)
    .option("--profile <name>", "cargo profile", defaultOptions.profile)
    .option("--bin <name>", "binary name", defaultOptions.bin)
    .option("--package <name>", "package name", defaultOptions.packageName)
    .option("--target <triple>", "Rust target triple (overrides --arch)")
    .option("--no-locked", "skip --locked")
    .option("--no-archive", "skip tar.gz packaging")
    .argument("[cargoArgs...]", "extra arguments passed to cargo zigbuild after --")
    .addHelpText(
      "after",
      `

Examples:
  pnpm build:linux -- --arch arm64
  pnpm build:linux -- --arch x64
  pnpm build:linux -- --arch arm64 --glibc 2.36
  pnpm build:linux -- --arch x64 --features some-feature

glibc baseline:
  Run "ldd --version" on the target machine and use the version from the first line.
  A lower baseline can run on more (older) Linux distributions.`,
    )
    .parse(["node", "scripts/build-linux.mts", ...argv]);

  const parsed = program.opts<{
    arch: Arch;
    archive: boolean;
    bin: string;
    glibc: string;
    locked: boolean;
    package: string;
    profile: string;
    target?: string;
  }>();

  if (parsed.arch !== "arm64" && parsed.arch !== "x64") {
    throw new Error(`Invalid --arch "${parsed.arch}": expected "arm64" or "x64".`);
  }

  return {
    arch: parsed.arch,
    archive: parsed.archive,
    bin: parsed.bin,
    cargoArgs: program.args,
    glibc: parsed.glibc,
    locked: parsed.locked,
    packageName: parsed.package,
    profile: parsed.profile,
    target: parsed.target ?? archTargets[parsed.arch],
  };
}

async function main(): Promise<void> {
  const options = parseArgs(process.argv.slice(2));
  const zigTarget = `${options.target}.${options.glibc}`;

  await ensureCommand("cargo", ["--version"], "Install Rust with rustup.");
  await ensureCommand("rustup", ["--version"], "Install Rust with rustup.");
  await ensureCommand("zig", ["version"], "Install Zig, for example: brew install zig.");
  await ensureCommand(
    "cargo",
    ["zigbuild", "--help"],
    "Install cargo-zigbuild: cargo install cargo-zigbuild.",
  );

  await run("rustup", ["target", "add", options.target]);

  const cargoArgs = [
    "zigbuild",
    "-p",
    options.packageName,
    "--bin",
    options.bin,
    "--profile",
    options.profile,
    "--target",
    zigTarget,
  ];

  if (options.locked) {
    cargoArgs.push("--locked");
  }

  cargoArgs.push(...options.cargoArgs);
  await run("cargo", cargoArgs);

  const binaryPath = await findBuiltBinary(options, zigTarget);
  const releaseName = buildReleaseName(options);
  const outputDir = join(repoRoot, "dist", releaseName);
  const outputBinary = join(outputDir, options.bin);
  const checksumPath = join(outputDir, "SHA256SUMS");

  await mkdir(outputDir, { recursive: true });
  await copyFile(binaryPath, outputBinary);
  await chmod(outputBinary, 0o755);

  const digest = await sha256(outputBinary);
  await writeFile(checksumPath, `${digest}  ${options.bin}\n`, "utf8");

  console.log(`Binary: ${outputBinary}`);
  console.log(`SHA256: ${digest}`);

  if (options.archive) {
    const archivePath = join(repoRoot, "dist", `${releaseName}.tar.gz`);
    await run(
      "tar",
      ["-czf", archivePath, "--format", "ustar", "-C", join(repoRoot, "dist"), releaseName],
      { env: { COPYFILE_DISABLE: "1" } },
    );
    console.log(`Archive: ${archivePath}`);
  }
}

async function ensureCommand(command: string, args: string[], hint: string): Promise<void> {
  try {
    await run(command, args, { quiet: true });
  } catch {
    throw new Error(`Missing required command: ${command}\n${hint}`);
  }
}

type RunOptions = {
  env?: Record<string, string>;
  quiet?: boolean;
};

async function run(command: string, args: string[], options: RunOptions = {}): Promise<void> {
  const rendered = [command, ...args].join(" ");
  if (!options.quiet) {
    console.log(`$ ${rendered}`);
  }

  await new Promise<void>((resolvePromise, rejectPromise) => {
    const child = spawn(command, args, {
      cwd: repoRoot,
      env: { ...process.env, ...options.env },
      stdio: options.quiet ? "ignore" : "inherit",
    });

    child.on("error", rejectPromise);
    child.on("close", (code) => {
      if (code === 0) {
        resolvePromise();
        return;
      }
      rejectPromise(new Error(`${rendered} exited with status ${code}`));
    });
  });
}

async function findBuiltBinary(options: Options, zigTarget: string): Promise<string> {
  const profileDir = options.profile === "dev" ? "debug" : options.profile;
  const candidates = [
    join(repoRoot, "target", zigTarget, profileDir, options.bin),
    join(repoRoot, "target", options.target, profileDir, options.bin),
  ];

  for (const candidate of candidates) {
    try {
      await access(candidate, constants.X_OK);
      return candidate;
    } catch {
      // Try the next cargo-zigbuild output layout.
    }
  }

  throw new Error(
    `Built binary was not found. Checked:\n${candidates.map((path) => `  ${path}`).join("\n")}`,
  );
}

function buildReleaseName(options: Options): string {
  const targetLabel = options.target.replace("-unknown-linux-", "-linux-");
  return `coda-server-${targetLabel}-glibc-${options.glibc}`;
}

async function sha256(path: string): Promise<string> {
  const hash = createHash("sha256");
  const stream = createReadStream(path);

  return await new Promise<string>((resolvePromise, rejectPromise) => {
    stream.on("data", (chunk) => hash.update(chunk));
    stream.on("error", rejectPromise);
    stream.on("end", () => resolvePromise(hash.digest("hex")));
  });
}

main().catch((error: unknown) => {
  const message = error instanceof Error ? error.message : String(error);
  console.error(message);
  process.exit(1);
});
