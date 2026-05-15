<?php
// PHPUnit metadata refs fixture: DocBlock annotations と PHP attributes 経由の
// method 参照を astro-sight が解決できるか検証するためのサンプル。

declare(strict_types=1);

namespace Test\Sample;

use PHPUnit\Framework\TestCase;

final class SamplePhpUnitTest extends TestCase
{
    /**
     * @dataProvider providerForValidateFormat
     */
    public function testValidations(string $provided, bool $isValid): void
    {
        self::assertSame($isValid, str_contains($provided, '@'));
    }

    #[DataProvider('attrProvider')]
    public function testWithAttribute(int $a, int $b): void
    {
        self::assertSame($a, $b);
    }

    public function providerForValidateFormat(): array
    {
        return [
            'ok' => ['test@example.com', true],
            'ng' => ['no-at', false],
        ];
    }

    public function attrProvider(): array
    {
        return [[1, 1], [2, 2]];
    }
}
