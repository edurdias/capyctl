// Website spec, Docs: "CLI reference — generated from the clap definitions at
// build time, never hand-written."
const HEAD = /^# Command-Line Help for `capyctl`\n\nThis document contains the help content for the `capyctl` command-line program\.\n\n/;

export function cliPage(raw, { banner } = {}) {
  if (!HEAD.test(raw)) throw new Error('unexpected clap-markdown output');
  const body = raw.replace(HEAD, '');
  return [
    '---',
    'title: "CLI reference"',
    'description: "Every capyctl command, argument and option."',
    ...(banner ? ['banner:', `  content: ${JSON.stringify(banner)}`] : []),
    '---',
    '',
    'Generated from the command definitions of this release. `capyctl <command> --help` prints the same descriptions.',
    '',
    body,
  ].join('\n');
}
