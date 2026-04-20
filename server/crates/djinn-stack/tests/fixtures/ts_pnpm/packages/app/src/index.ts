export function greet(name: string): string {
    return `hello, ${name}`;
}

export function farewell(name: string): string {
    return `goodbye, ${name}`;
}

export const VERSION = "0.1.0";

export interface Person {
    first_name: string;
    last_name: string;
    age: number;
}

export type Greeting = { en: string; es: string; fr: string };
