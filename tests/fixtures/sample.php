<?php

namespace App\Service;

use App\Repository\UserRepository;

class UserService
{
    private UserRepository $repository;

    public function __construct(UserRepository $repository)
    {
        $this->repository = $repository;
    }

    public function findUser(int $id): ?array
    {
        return $this->repository->find($id);
    }

    public function createUser(string $name, string $email): array
    {
        return $this->repository->create($name, $email);
    }
}
