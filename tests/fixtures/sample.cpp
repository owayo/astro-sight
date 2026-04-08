#include <string>
#include <vector>
#include <iostream>

class StringPool {
public:
    void add(const std::string& str) {
        pool_.push_back(str);
    }

    const std::string& get(size_t index) const {
        return pool_.at(index);
    }

    size_t size() const {
        return pool_.size();
    }

private:
    std::vector<std::string> pool_;
};

int main() {
    StringPool pool;
    pool.add("hello");
    std::cout << pool.get(0) << std::endl;
    return 0;
}
