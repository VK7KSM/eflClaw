import CodeMirror from '@uiw/react-codemirror';
import { StreamLanguage } from '@codemirror/language';
import { toml } from '@codemirror/legacy-modes/mode/toml';
import { oneDark } from '@codemirror/theme-one-dark';
import { EditorView } from '@codemirror/view';

interface Props {
  rawToml: string;
  onChange: (raw: string) => void;
  disabled?: boolean;
}

export default function ConfigRawEditor({ rawToml, onChange, disabled }: Props) {
  return (
    <div className="bg-gray-900 rounded-xl border border-gray-800 overflow-hidden">
      <div className="flex items-center justify-between px-4 py-2 border-b border-gray-800 bg-gray-800/50">
        <span className="text-xs text-gray-400 font-medium uppercase tracking-wider">
          TOML Configuration
        </span>
        <span className="text-xs text-gray-500">
          {rawToml.split('\n').length} lines
        </span>
      </div>
      <CodeMirror
        value={rawToml}
        onChange={onChange}
        readOnly={disabled}
        theme={oneDark}
        extensions={[
          StreamLanguage.define(toml),
          EditorView.lineWrapping,
        ]}
        minHeight="500px"
        basicSetup={{
          lineNumbers: true,
          foldGutter: true,
          highlightActiveLine: true,
          bracketMatching: true,
        }}
      />
    </div>
  );
}
