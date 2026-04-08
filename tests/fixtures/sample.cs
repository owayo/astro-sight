using System;
using System.Collections.Generic;

namespace Example
{
    public class Calculator
    {
        private readonly List<double> history = new();

        public double Add(double a, double b)
        {
            var result = a + b;
            history.Add(result);
            return result;
        }

        public double Multiply(double a, double b)
        {
            var result = a * b;
            history.Add(result);
            return result;
        }

        public IReadOnlyList<double> GetHistory()
        {
            return history.AsReadOnly();
        }
    }
}
