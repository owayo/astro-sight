import { readFileSync } from "node:fs";

export interface Config {
	host: string;
	port: number;
}

export class AppServer {
	private config: Config;

	constructor(config: Config) {
		this.config = config;
	}

	getAddress(): string {
		return `${this.config.host}:${this.config.port}`;
	}

	loadFile(path: string): string {
		return readFileSync(path, "utf-8");
	}
}

export function createServer(host: string, port: number): AppServer {
	return new AppServer({ host, port });
}
