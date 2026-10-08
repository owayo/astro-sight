export function spacing(scale = 1): { gap: number } { return { gap: scale * 12 }; }
export function optional(size: number, scale?: number): number { return size + (scale ?? 1); }
import { spacing as local } from './spacing';
import { 'spacing' as named } from './spacing';
import * as helpers from './spacing';
export { spacing as forwarded } from './spacing';
export * from './spacing';
spacing(/* ordinary comment */);
spacing?.();
spacing<string>();
new spacing();
spacing(...values);
const view = <Spacing />;
const modules = import.meta.glob('./*.ts');
const load = require;
load('./spacing');
import('./spacing');
require('./spacing');
type Exported = typeof import('./spacing');
declare module './spacing' { export function extra(): void; }
eval('spacing(2)');
new Function('spacing(2)')();
globalThis['eval']('spacing(2)');
spac\u0069ng(2);
ev\u0061l('spacing(2)');
requ\u0069re('./spacing');
