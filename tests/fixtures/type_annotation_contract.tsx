export const AREA: Dim = { width: 100 * 2, height: 50 * 2 };
export const LITERALS: Shape = {
  text: 'a  b',
  template: `a  b`,
  pattern: /a  b/g,
  view: <span>a  b</span>,
};
export const HANDLER: Handler = (value: number): number => value;
export const ASI: Handler = () => { return
  { value: 1 }; };
