"""Session setup dependencies for runnable selected rows (not qualification)."""
import argparse
import v020

ALIASES = {'vllm032': 'VLLM_032', 'vllm033': 'VLLM_033', 'sglang0522': 'SGLANG_0522',
           'sglang0523': 'SGLANG_0523', 'llamacpp090': 'LLAMACPP', 'tensorfold126': 'TENSORFOLD_126'}
EMBED = {'VLLM_033', 'SGLANG_0523', 'LLAMACPP'}
TRACE = {'VLLM_033_TRACING', 'SGLANG_0523_TRACING'}


def dependencies(selection='all', unified=False):
    needed = set()
    for row in v020.select_rows(selection):
        if v020.skip_reason(row, unified=unified) or row['criteria'].startswith('not implemented:'):
            continue
        key = row['id'].upper()
        engines, models = set(), set()
        if row['module'] == 'engines':
            engines.add(ALIASES.get(row['engine'], row['engine']))
            models.add('CLEF' if row.get('decisions') else ('TENSORFOLD' if row['engine'] == 'tensorfold126' else 'CHAT'))
        elif key.startswith('LC'):
            engines = {'LMCACHE', 'LMCACHE_SGLANG'}
            if key in {'LC13', 'LC14', 'LC15', 'LC16', 'LC13U', 'LC14U', 'LC15U', 'LC16U'}:
                engines.add('LMCACHE_VLLM_034')
            models.add('CHAT')
            if key in {'LC18', 'LC12'}:
                needed.add('CAPYCTL_' + key)
        elif key.startswith('EM'):
            engines = {'EM6_VLLM'} if key == 'EM6' else EMBED.copy()
            if key == 'EM9':
                engines.discard('LLAMACPP')
            models.add('EMBED')
            if key == 'EM7':
                models.add('CHAT')
            if key == 'EM8':
                models.add('EMBED8')
        elif key.startswith('OT'):
            if key == 'OT11':
                engines = {'VLLM_033', 'SGLANG_0523'}
            elif key == 'OT10':
                engines = {'SGLANG_0523_SDK_FREE', 'SGLANG_0523_TRACING'}
            elif key == 'OT2':
                engines = {'SGLANG_0523', 'SGLANG_0523_TRACING'}
            elif key == 'OT1':
                engines = EMBED | {'SGLANG_0523_TRACING'}
            elif key == 'OT4':
                engines = {'VLLM_033', 'SGLANG_0523', 'SGLANG_0523_TRACING'}
            elif key == 'OT7':
                engines = {'VLLM_033', 'SGLANG_0523_TRACING'}
            else:
                engines = TRACE.copy()
                if key == 'OT8':
                    engines |= EMBED
                if key == 'OT5':
                    engines.add('LLAMACPP')
            if key != 'OT11':
                models.add('CHAT')
            if key in {'OT2', 'OT3'}:
                models.add('CLEF')
        elif key.startswith('TF'):
            number = int(key[2:])
            engines = {'TENSORFOLD_126'} if number <= 3 else {'TENSORFOLD_100' if number < 11 else 'TENSORFOLD_115'}
            if number == 11:
                engines.add('TENSORFOLD_126')
            if number in {3, 9, 12}:
                engines.add('TENSORFOLD_066')
            if number in {4, 8}:
                engines.add('VLLM_032')
            if number in {4, 8}:
                models.add('CHAT')
            if number in {5, 13}:
                engines.add('STRACE')
            if number not in {1, 11}:
                models.add('TENSORFOLD')
        else:
            engines = {'ISSUE225': {'VLLM_032', 'VLLM_033'},
                       'ISSUE236': {'SGLANG_0522', 'SGLANG_0523'},
                       'ISSUE235': {'SGLANG_EDITABLE', 'VLLM_033'},
                       'ISSUE240': {'VLLM_033'}}[key]
            models.add('CHAT')
            if key in {'ISSUE235', 'ISSUE240'}:
                needed.add('CAPYCTL_OLD')
        if 'LMCACHE' in engines:
            engines.add('VLLM_033')
        if 'LMCACHE_SGLANG' in engines:
            engines.add('SGLANG_0523')
        if 'LMCACHE_VLLM_034' in engines:
            engines.add('VLLM_034')
        needed |= engines
        for model in models:
            if engines != {'LLAMACPP'}:
                needed.add('MODEL_' + model)
            if 'LLAMACPP' in engines and model in {'CHAT', 'CLEF', 'EMBED'}:
                needed.add('MODEL_' + model + '_GGUF')
    return needed


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--only', default='all')
    parser.add_argument('--unified', action='store_true')
    args = parser.parse_args()
    print('\n'.join(sorted(dependencies(args.only, args.unified))))
