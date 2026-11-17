import english from './locales/en.json';
import type { Appearance } from '../contracts';

let preference: Appearance['language'] = 'system';
export const locale = () => preference === 'zh-CN' || (preference === 'system' && navigator.language.toLowerCase().startsWith('zh')) ? 'zh-CN' : 'en';
export function setLanguage(value: Appearance['language']) {
  preference = value;
  document.documentElement.lang = locale();
}
// Only source literals pass through this function, BEFORE dynamic/user values are interpolated.
// Source-language keys keep translations local; protocol fields and raw diagnostics are untouched.
export function t(source: string): string {
  if (locale() === 'zh-CN') return source;
  return source.replace(/[^<>"']*[\p{Script=Han}][^<>"']*/gu, part => {
    const key = part.trim();
    const translated = (english as Record<string, string>)[key];
    return translated === undefined ? part : part.replace(key, translated);
  });
}

export function html(strings: TemplateStringsArray, ...values: unknown[]): string {
  return strings.reduce((result, part, index) => result + t(part) + (index < values.length ? String(values[index]) : ''), '');
}
