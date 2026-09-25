// Website spec, Docs: "CLI reference — generated from the clap definitions at
// build time, never hand-written."
const HEAD = /^# Command-Line Help for `mllm`\n\nThis document contains the help content for the `mllm` command-line program\.\n\n/;

export function cliPage(raw) {
  if (!HEAD.test(raw)) throw new Error('unexpected clap-markdown output');
  const body = raw.replace(HEAD, '');
  return [
    '---',
    'title: "CLI reference"',
    'description: "Every mllm command, argument and option."',
    '---',
    '',
    'Generated from the command definitions of this release. `mllm <command> --help` prints the same text.',
    '',
    body,
  ].join('\n');
}
