export class DurableObject<E = unknown> {
  constructor(
    public ctx: unknown,
    public env: E,
  ) {}
}
