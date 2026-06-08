#!/usr/bin/env node
import { runCli } from "../src/cli/main.js";

runCli(process.argv.slice(2)).then((code) => process.exit(code));
