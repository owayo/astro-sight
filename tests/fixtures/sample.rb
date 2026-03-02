require "json"
require_relative "lib/helper"

module MyApp
  class User
    def initialize(name, age)
      @name = name
      @age = age
    end

    def greet
      puts "Hello, #{@name}!"
    end

    def self.create(name, age)
      new(name, age)
    end
  end

  class Admin < User
    def admin?
      true
    end
  end

  def self.run
    user = User.create("Alice", 30)
    user.greet
  end
end
