import { defaultKeymap, history, historyKeymap } from '@codemirror/commands';
import { yaml } from '@codemirror/lang-yaml';
import { defaultHighlightStyle, syntaxHighlighting } from '@codemirror/language';
import { EditorState } from '@codemirror/state';
import { EditorView, keymap, lineNumbers } from '@codemirror/view';

// ADR 0040 §13 (plan KTD14): CodeMirror 6 with the YAML language, loaded
// only when the YAML tab opens, so it lands in its own chunk.
export interface Editor {
  setText(text: string): void;
  destroy(): void;
}

export function createEditor(parent: HTMLElement, text: string, onChange: (text: string) => void): Editor {
  const view = new EditorView({
    parent,
    state: EditorState.create({
      doc: text,
      extensions: [
        lineNumbers(),
        history(),
        keymap.of([...defaultKeymap, ...historyKeymap]),
        yaml(),
        syntaxHighlighting(defaultHighlightStyle),
        EditorView.lineWrapping,
        EditorView.contentAttributes.of({ 'aria-label': 'Deployment file (YAML)' }),
        EditorView.updateListener.of((update) => {
          if (update.docChanged) {
            onChange(update.state.doc.toString());
          }
        }),
      ],
    }),
  });
  return {
    setText(next) {
      const current = view.state.doc.toString();
      if (next !== current) {
        view.dispatch({ changes: { from: 0, to: current.length, insert: next } });
      }
    },
    destroy: () => view.destroy(),
  };
}
